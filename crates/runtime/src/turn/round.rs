//! 主循环每一轮用到的小工具：工具结果收尾、本回合/本步消息切片、重复调用提示、对工具调用的扫描。

use super::request::awaits_result;
use super::{TurnError, TurnOutcome};
use crate::agent::Agent;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use std::path::PathBuf;

/// Count StructuredOutput tool uses in the current session messages.
pub(super) fn count_structured_output_calls(
    messages: &[base::interface::model::ModelMessage],
) -> u32 {
    messages
        .iter()
        .filter(|m| {
            m.role == base::interface::model::MessageRole::Assistant
                && m.content.iter().any(|b| {
                    matches!(b, base::interface::model::ModelContentBlock::ToolUse { name, .. } if name == "StructuredOutput")
                })
        })
        .count() as u32
}

impl Agent {
    pub(super) async fn handle_tool_result(
        &mut self,
        tool_use_id: String,
        content: String,
        is_error: bool,
    ) -> Result<TurnOutcome, TurnError> {
        if !awaits_result(self.session.messages(), &tool_use_id) {
            return Err(TurnError::Internal(format!(
                "no tool call {tool_use_id} is waiting for a result"
            )));
        }
        self.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: Some(is_error),
            }],
        });
        Ok(TurnOutcome::stopped("tool_result_received"))
    }

    /// The messages pushed since this turn's user message, that message
    /// included; the whole transcript before any turn has started.
    pub(super) fn messages_this_turn(&self) -> &[ModelMessage] {
        let messages = self.session.messages();
        let Some(started) = self.turn.started_at else {
            return messages;
        };
        let first = self
            .session
            .message_ages()
            .iter()
            .rposition(|(_, at)| *at < started)
            .map_or(0, |i| i + 1);
        &messages[first..]
    }

    /// The reply of the step that just ran and the results after it.
    pub(super) fn this_step(&self) -> &[ModelMessage] {
        let messages = self.session.messages();
        let reply = messages
            .iter()
            .rposition(|m| m.role == MessageRole::Assistant)
            .unwrap_or(0);
        &messages[reply..]
    }

    /// The `file_path` of every `Read`/`Write`/`Edit` call in `messages`.
    pub(super) fn extract_tool_file_paths(messages: &[ModelMessage]) -> Vec<PathBuf> {
        let tool_names = ["Read", "Write", "Edit"];
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for msg in messages {
            for block in &msg.content {
                if let ModelContentBlock::ToolUse { name, input, .. } = block {
                    if !tool_names.contains(&name.as_str()) {
                        continue;
                    }
                    if let Some(fp) = input.get("file_path").and_then(|v| v.as_str()) {
                        let path = PathBuf::from(fp);
                        if seen.insert(path.clone()) {
                            paths.push(path);
                        }
                    }
                }
            }
        }
        paths
    }

    /// Counts the calls of the step that just ran — see [`crate::repeat_guard`].
    pub(super) fn notice_repeated_calls(&mut self) {
        let Some(step) = self
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
        else {
            return;
        };
        let turn = self.session.turn_count;
        let notices: Vec<String> = step
            .content
            .iter()
            .filter_map(|block| match block {
                ModelContentBlock::ToolUse { name, input, .. } => self
                    .repeat_guard
                    .record(turn, name, input)
                    .map(|count| (name.clone(), count)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|(name, count)| {
                crate::repeat_guard::notice(&name, count, self.scene.interactive())
            })
            .collect();
        for text in notices {
            self.session.push_message(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text { text }],
            });
        }
    }

    /// Whether any `Bash` call in `messages` looks like it mutated git state
    /// — used to decide whether to pay for a `FrozenContext::refresh_git()`
    /// round-trip (real subprocess I/O, not free) rather than trusting the
    /// session-start snapshot for the rest of a long-running session (see
    /// `crates/core/src/frozen/mod.rs`'s module-level "not refreshed during a
    /// session" caveat).
    ///
    /// Deliberately a heuristic, not a shell parser: scans for `git`
    /// followed within a few tokens by a known state-mutating subcommand
    /// name, tolerating global flags (`-C <dir>`, `--no-pager`, ...) between
    /// them. False positives (e.g. `git log --grep=commit`) just cost one
    /// extra (cheap, timeout-bounded) `refresh_git()` call; false negatives
    /// (a mutating command this list doesn't recognize, or one run through
    /// e.g. a wrapper script) just mean the "frozen until next session"
    /// staleness persists a bit longer — neither is a correctness hazard.
    pub(super) fn included_git_mutating_bash_call(messages: &[ModelMessage]) -> bool {
        const GIT_MUTATING_SUBCOMMANDS: &[&str] = &[
            "checkout",
            "switch",
            "commit",
            "merge",
            "rebase",
            "reset",
            "revert",
            "cherry-pick",
            "stash",
            "pull",
            "add",
            "rm",
            "mv",
            "restore",
            "branch",
            "worktree",
            "init",
            "clone",
            "am",
            "apply",
        ];
        for msg in messages {
            for block in &msg.content {
                let ModelContentBlock::ToolUse { name, input, .. } = block else {
                    continue;
                };
                if name != "Bash" {
                    continue;
                }
                let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
                    continue;
                };
                for (idx, _) in command.match_indices("git") {
                    // Require a word boundary before "git" so e.g. "digit"
                    // doesn't match.
                    if idx > 0 && command.as_bytes()[idx - 1].is_ascii_alphanumeric() {
                        continue;
                    }
                    let after: Vec<&str> = command[idx + 3..].split_whitespace().take(6).collect();
                    if after.iter().any(|t| GIT_MUTATING_SUBCOMMANDS.contains(t)) {
                        return true;
                    }
                }
            }
        }
        false
    }
}
